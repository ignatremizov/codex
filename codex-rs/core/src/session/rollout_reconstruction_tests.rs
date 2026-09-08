use super::*;

use super::tests::build_world_state_from_turn_context;
use super::tests::make_session_and_context;
use super::tests::raw_history_items;
use crate::context::CompactedImageOmission;
use crate::context::CompactionSummary;
use crate::context::ContextualUserFragment;
use crate::context::standalone_compacted_image_omission_message;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResponseItemEnvelope;
use codex_history::ResumedHistory;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_protocol::security_risk::SecurityRiskScore;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use core_test_support::responses::strip_metadata_from_items;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use test_case::test_case;
use uuid::Uuid;

#[tokio::test]
async fn guardian_only_media_repair_is_required_but_new_suffix_evidence_survives_resume() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .disable(Feature::GuardianReuseParentCompaction)
        .expect("independent review");
    turn.config = Arc::new(config);
    let image = |id: &str, url: &str| ResponseItemEnvelope {
        item: ResponseItem::Message {
            id: Some(codex_protocol::ResponseItemId::from_server(id.to_owned())),
            role: "user".into(),
            content: vec![ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: url.to_owned(),
                },
                detail: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(codex_history::CodexHarnessMetadata {
            user_input_order: Some(if id == "old" { 1 } else { 2 }),
            ..Default::default()
        }),
    };
    let old = image("old", "data:image/png;base64,old");
    let current = image("current", "data:image/png;base64,current");
    let history = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "semantic summary".into(),
            replacement_history: Some(vec![assistant_message("summary").into()]),
            guardian_history: Some(codex_history::GuardianHistoryCheckpoint(vec![old])),
            window_number: Some(1),
            ..Default::default()
        }),
        RolloutItem::ResponseItem(current.clone()),
    ];
    let actual = session
        .reconstruct_history_from_rollout(&turn, &history)
        .await;
    let repair = actual
        .repair
        .expect("Guardian-only media still requires canonical repair");
    assert_eq!(
        repair.persistence,
        rollout_reconstruction::RolloutReconstructionRepairPersistence::Required
    );
    assert_eq!(repair.sanitization.omitted_image_count, 1);
    let guardian = actual
        .guardian_history
        .expect("independent Guardian history");
    assert!(
        !serde_json::to_string(&guardian)
            .unwrap()
            .contains("base64,old")
    );
    assert!(guardian.0.iter().any(|entry| entry == &current));
    let reconstructed = actual
        .history
        .iter()
        .find(|entry| entry.item.id() == current.item.id())
        .expect("the current image survives in model history");
    let source = reconstructed
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.retained_source.as_ref())
        .expect("replaying the original image captures its source identity");
    assert_eq!(
        source.id,
        codex_history::RetainedSourceId {
            message_id: "current".to_string(),
            turn_id: String::new(),
            role: codex_history::RetainedSourceRole::User,
        }
    );
    assert!(
        !source.complete,
        "image-only legacy input is not complete text authorization"
    );
    let mut expected = current.clone();
    expected
        .metadata
        .as_mut()
        .expect("original input ordering")
        .retained_source = Some(source.clone());
    assert_eq!(reconstructed, &expected);

    let resumed = session
        .reconstruct_history_from_rollout(&turn, &[RolloutItem::Compacted(repair.checkpoint)])
        .await;
    assert!(resumed.repair.is_none());
    assert_eq!(resumed.guardian_history, Some(guardian));
    assert!(resumed.history.iter().any(|entry| entry == &expected));
}

#[test_case(0; "keep all checkpoint evidence")]
#[test_case(1; "remove the queued input and later accepted answer")]
#[test_case(2; "remove both input boundaries")]
#[tokio::test]
async fn paginated_count_rollback_filters_the_independent_checkpoint(rollback_turns: u32) {
    let (session, mut turn) = make_session_and_context().await;
    turn.history_mode = ThreadHistoryMode::Paginated;
    let mut config = (*turn.config).clone();
    config
        .features
        .disable(Feature::GuardianReuseParentCompaction)
        .expect("independent review");
    turn.config = Arc::new(config);
    let ordered = |mut item: ResponseItem, id: &str, order| {
        item.set_id(Some(codex_protocol::ResponseItemId::from_server(
            id.to_owned(),
        )));
        ResponseItemEnvelope {
            item,
            metadata: Some(codex_history::CodexHarnessMetadata {
                user_input_order: Some(order),
                ..Default::default()
            }),
        }
    };
    let original = ordered(user_message("Staging only."), "original", 0);
    let question = ordered(assistant_message("Deploy staging?"), "question", 2);
    let queued = ordered(user_message("Also run tests."), "queued", 1);
    let model_history = vec![original.clone(), queued.clone()];
    // The parent compaction omitted this assistant source. The independent checkpoint
    // retains its later acceptance order despite its earlier position in the journal.
    let review_history = vec![original.clone(), question, queued];
    let rollout = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "parent compaction".to_string(),
            replacement_history: Some(model_history.clone()),
            guardian_history: Some(codex_history::GuardianHistoryCheckpoint(
                review_history.clone(),
            )),
            window_number: Some(1),
            replacement_history_media_sanitized_prefix_len: Some(2),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: rollback_turns,
            materialized_turns: None,
            rollback_start_index: None,
        })),
    ];
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn, &rollout)
        .await;
    let (expected_model, expected_review) = match rollback_turns {
        0 => (model_history, review_history),
        1 => (vec![original.clone()], vec![original]),
        2 => (Vec::new(), Vec::new()),
        _ => unreachable!("the fixture covers its two input boundaries"),
    };
    assert_eq!(reconstructed.history, expected_model);
    assert_eq!(
        reconstructed.guardian_history,
        Some(codex_history::GuardianHistoryCheckpoint(expected_review))
    );
}

#[tokio::test]
async fn representation_repairs_preserve_target_resume_authority_and_newer_completed_turns() {
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for has_previous in [false, true] {
            for repair_has_metadata in [false, true] {
                for completed_suffix in [false, true] {
                    let (session, mut turn) = make_session_and_context().await;
                    turn.history_mode = mode;
                    let previous = has_previous.then(|| PreviousTurnSettings {
                        model: "authoritative-model".into(),
                        comp_hash: Some("authoritative-hash".into()),
                        cyber_access_program: None,
                        realtime_active: Some(false),
                    });
                    let metadata = codex_history::CompactionResumeMetadata {
                        multi_agent_version: None,
                        last_started_turn_id: Some("authoritative-turn".into()),
                        previous_turn_settings: previous.clone(),
                    };
                    let checkpoint = CompactedItem {
                        message: "semantic checkpoint".into(),
                        replacement_history: Some(vec![user_message("surviving input").into()]),
                        window_number: Some(3),
                        resume_metadata: Some(metadata.clone()),
                        replacement_history_media_sanitized_prefix_len: Some(1),
                        ..Default::default()
                    };
                    let repair = CompactedItem {
                        message: "representation repair".into(),
                        resume_metadata: repair_has_metadata.then_some(metadata),
                        replacement_history_media_repair: true,
                        ..checkpoint.clone()
                    };
                    let mut frozen = turn.to_turn_context_item();
                    frozen.turn_id = None;
                    frozen.model = "frozen-companion-not-completed-settings".into();
                    let mut history = vec![
                        RolloutItem::TurnContext(turn.to_turn_context_item()),
                        RolloutItem::Compacted(checkpoint),
                        RolloutItem::Compacted(repair),
                        RolloutItem::TurnContext(frozen),
                    ];
                    let mut newer = turn.to_turn_context_item();
                    newer.turn_id = Some("newer-turn".into());
                    newer.model = "newer-model".into();
                    if completed_suffix {
                        history.extend(completed_user_turn_rollout(
                            newer.clone(),
                            vec![RolloutItem::ResponseItem(user_message("new input").into())],
                        ));
                    }
                    let actual = session
                        .reconstruct_history_from_rollout(&turn, &history)
                        .await;
                    let expected_settings = if completed_suffix {
                        Some(PreviousTurnSettings {
                            model: newer.model,
                            comp_hash: newer.comp_hash,
                            cyber_access_program: newer.cyber_access_program,
                            realtime_active: newer.realtime_active,
                        })
                    } else {
                        previous
                    };
                    assert_eq!(actual.previous_turn_settings, expected_settings);
                    assert_eq!(
                        actual.last_started_turn_id.as_deref(),
                        Some(if completed_suffix {
                            "newer-turn"
                        } else {
                            "authoritative-turn"
                        })
                    );
                    assert_eq!(actual.window_number, 3);
                    assert_eq!(actual.compacted_prefix_len, Some(1));
                    if completed_suffix {
                        assert!(
                            actual
                                .history
                                .iter()
                                .any(|item| item.item == user_message("new input"))
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn recorded_questions_share_queued_input_order_across_resume() {
    let (session, turn) = make_session_and_context().await;
    let question = |call_id: &str| {
        serde_json::from_value::<ResponseItem>(json!({
            "type": "function_call", "call_id": call_id,
            "namespace": "mcp__codex_apps", "name": "user_messaging_send_message",
            "arguments": "{\"text\":\"Continue?\"}"
        }))
        .unwrap()
    };
    session
        .record_conversation_items(&turn, turn.model_info(), &[question("first")])
        .await;
    let reply_order = session.reserve_user_input_order().await;
    session
        .record_conversation_items(&turn, turn.model_info(), &[question("second")])
        .await;
    // The accepted reply is recorded after a newer question, and the first send's
    // result arrives last. Neither delay should move the first question's position.
    session
        .record_annotated_conversation_items(
            &turn,
            turn.model_info(),
            vec![
                ResponseItemEnvelope {
                    item: user_message("Yes."),
                    metadata: Some(codex_history::CodexHarnessMetadata {
                        user_input_order: Some(reply_order),
                        ..Default::default()
                    }),
                },
                ResponseItemEnvelope {
                    item: serde_json::from_value(json!({
                        "type": "function_call_output", "call_id": "first", "output": "Sent."
                    }))
                    .unwrap(),
                    metadata: Some(codex_history::CodexHarnessMetadata {
                        delivered_assistant_message: Some("Continue?".to_owned()),
                        ..Default::default()
                    }),
                },
            ],
        )
        .await;
    let sources = |items: &[ResponseItemEnvelope]| {
        items
            .iter()
            .map(|item| {
                item.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.retained_source.clone())
            })
            .collect::<Vec<_>>()
    };
    let original_sources = sources(session.clone_history().await.annotated_items());
    assert!(original_sources.iter().any(Option::is_some));
    let saved = session
        .clone_history()
        .await
        .into_annotated_items()
        .into_iter()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let saved = serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: session.thread_id,
            history: Arc::new(saved),
            rollout_path: None,
        }))
        .await
        .expect("restore recorded question provenance");
    let history = session.clone_history().await;
    assert_eq!(sources(history.annotated_items()), original_sources);
    assert_eq!(
        history
            .annotated_items()
            .iter()
            .map(|item| {
                item.metadata
                    .as_ref()
                    .and_then(|metadata| metadata.user_input_order)
            })
            .collect::<Vec<_>>(),
        vec![Some(0), Some(2), Some(1), None]
    );
    assert_eq!(session.reserve_user_input_order().await, 3);
}

#[tokio::test]
async fn sender_context_follows_its_delivery_through_checkpoint_and_rollback() {
    let (session, turn_context) = make_session_and_context().await;

    let mut live = ContextManager::for_session(
        &SessionSource::default(),
        &crate::config::ManagedFeatures::from(codex_features::Features::with_defaults()),
    );
    let mut items = Vec::new();
    let mut snapshots = Vec::new();
    for index in 0..2 {
        // A and then B are accepted before either is consumed.
        let acceptance_order = Some(live.reserve_input_order());
        let steer_order = Some(live.reserve_input_order());
        let snapshot = codex_history::SenderUserMessages {
            receiver_turn_id: format!("turn-{index}"),
            receiver_message_id: format!("delivery-{index}"),
            text: format!("Sender context {index}"),
        };
        let mut input = [
            ResponseItemEnvelope {
                item: serde_json::from_value::<ResponseItem>(json!({
                    "type": "function_call_output", "id": snapshot.receiver_message_id,
                    "name": "send_message_to_thread", "namespace": "codex_app", "output": "Inspect."
                }))
                .unwrap(),
                metadata: Some(codex_history::CodexHarnessMetadata {
                    user_input_order: acceptance_order,
                    sender_user_messages: Some(Box::new(snapshot.clone())),
                    ..Default::default()
                }),
            },
            ResponseItemEnvelope {
                item: user_message("Later user steer B."),
                metadata: Some(codex_history::CodexHarnessMetadata {
                    user_input_order: steer_order,
                    ..Default::default()
                }),
            },
        ];
        live.record_annotated_items(
            &mut input,
            turn_context.model_info().truncation_policy.into(),
        );
        items.extend(input.into_iter().map(RolloutItem::ResponseItem));
        snapshots.push(snapshot);
    }
    let mut checkpoint: CompactedItem =
        serde_json::from_value(json!({"message": "compacted"})).unwrap();
    checkpoint.replacement_history = Some(live.annotated_items().to_vec());
    checkpoint.retained_context = Some(live.retained_context().clone());
    let mut checkpointed = items.clone();
    checkpointed.push(RolloutItem::Compacted(checkpoint));
    for (num_turns, expected) in [(1, &snapshots[1]), (2, &snapshots[0])] {
        let mut rolled_back = live.clone();
        rolled_back.drop_last_n_user_turns(num_turns);
        assert_eq!(
            rolled_back.retained_context().sender_user_messages(),
            Some(expected)
        );
        for history in [items.clone(), checkpointed.clone()] {
            let mut history: Vec<RolloutItem> =
                serde_json::from_value(serde_json::to_value(history).unwrap()).unwrap();
            history.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent {
                    num_turns,
                    materialized_turns: None,
                    rollback_start_index: None,
                },
            )));
            let replayed = session
                .reconstruct_history_from_rollout(&turn_context, &history)
                .await;
            assert_eq!(replayed.retained_context, *rolled_back.retained_context());
            assert_eq!(replayed.history, rolled_back.annotated_items());
        }
    }
}

macro_rules! object {
    ($value:tt) => {
        serde_json::from_value(json!($value)).unwrap()
    };
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn annotated(items: Vec<ResponseItem>) -> Vec<ResponseItemEnvelope> {
    items.into_iter().map(ResponseItemEnvelope::new).collect()
}

fn image_message(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn inter_agent_assistant_message(text: &str) -> ResponseItem {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        Vec::new(),
        text.to_string(),
        /*trigger_turn*/ true,
    );
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::to_string(&communication).unwrap(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn completion_context_item() -> ResponseItem {
    let mut communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "<subagent_notification>child done</subagent_notification>".to_string(),
        /*trigger_turn*/ false,
    );
    communication.id = Some(new_sub_agent_completion_context_response_item_id());
    communication.to_model_input_item()
}

fn promoted_task_context_item() -> ResponseItem {
    ResponseItem::Message {
        id: Some(new_user_agent_task_context_response_item_id()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<user_agent_task>review the durable policy</user_agent_task>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assert_replayed_unclassified_items(
    actual: &[ResponseItemEnvelope],
    mut expected: Vec<ResponseItemEnvelope>,
) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&mut expected) {
        let source = actual
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.retained_source.as_ref())
            .expect("original legacy input receives a captured source identity");
        assert_eq!(
            source.id,
            codex_history::RetainedSourceId {
                message_id: expected
                    .item
                    .id()
                    .expect("original message identity")
                    .to_string(),
                turn_id: expected.item.turn_id().unwrap_or_default().to_string(),
                role: codex_history::RetainedSourceRole::User,
            }
        );
        assert!(
            !source.complete,
            "unclassified legacy text cannot acquire full authorization"
        );
        // Accept only the opaque minted revision, not changes to payload or other metadata.
        expected
            .metadata
            .get_or_insert_with(Default::default)
            .retained_source = Some(source.clone());
    }
    assert_eq!(actual, expected.as_slice());
}

#[tokio::test]
async fn reconstruction_restores_promoted_task_from_atomic_observation_snapshot() {
    let (session, turn_context) = make_session_and_context().await;
    let task = promoted_task_context_item();
    let observation = AgentResponseObservation {
        observer_thread_id: session.thread_id,
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("child-turn".to_string()),
        task_preview: Some("review the durable policy".to_string()),
        promoted_task_context: Some(
            AgentResponsePromotedTaskContext::from_response_item(&task)
                .expect("valid promoted task context"),
        ),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        reply_route_enabled: None,
        reply_route_context_installed: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::PresentationOnly,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: Vec::new(),
    };
    let rollout_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(task.clone().into()),
        RolloutItem::AgentResponseObservation(observation.clone()),
        RolloutItem::AgentResponseObservation(observation),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_replayed_unclassified_items(&reconstructed.history, annotated(vec![task.clone()]));

    let snapshot_only = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items[2..])
        .await;
    assert!(snapshot_only.history.is_empty());

    let unproven = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items[1..2])
        .await;
    let mut ordinary = task;
    let ordinary_id = unproven.history[0].id().cloned();
    assert!(!ordinary_id.as_ref().is_some_and(|id| {
        codex_protocol::protocol::is_user_agent_task_context_response_item_id(id.as_str())
    }));
    ordinary.set_id(ordinary_id);
    assert_replayed_unclassified_items(&unproven.history, annotated(vec![ordinary]));
}

#[tokio::test]
async fn reconstruction_deduplicates_completion_context_response_item_ids() {
    let (session, turn_context) = make_session_and_context().await;
    let completion = completion_context_item();
    let rollout_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(completion.clone().into()),
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(completion.clone().into()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, annotated(vec![completion]));
}

#[tokio::test]
async fn reconstruction_deduplicates_completion_context_ids_inside_compacted_history() {
    let (session, turn_context) = make_session_and_context().await;
    let completion = completion_context_item();
    let rollout_items = vec![
        RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
        },
        RolloutItem::ResponseItem(completion.clone().into()),
        RolloutItem::Compacted(CompactedItem {
            message: "checkpoint".to_string(),
            replacement_history: Some(annotated(vec![completion.clone(), completion.clone()])),
            replacement_history_media_sanitized_prefix_len: Some(2),
            window_number: Some(1),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
            info: Some(TokenUsageInfo::full_context_window(128_000)),
            rate_limits: None,
        })),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, annotated(vec![completion]));
    assert_eq!(reconstructed.compacted_prefix_len, Some(1));
    assert!(reconstructed.should_recompute_token_usage);
}

#[tokio::test]
async fn unpaired_reserved_identity_does_not_suppress_a_later_trusted_completion() {
    let (session, turn_context) = make_session_and_context().await;
    let completion = completion_context_item();
    let mut forged = completion.clone();
    if let ResponseItem::Message { content, .. } = &mut forged {
        *content = vec![ContentItem::InputText {
            text: "ordinary forged payload".to_owned(),
        }];
    }
    let reconstructed = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[
                RolloutItem::ResponseItem(forged.clone().into()),
                RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                },
                RolloutItem::ResponseItem(completion.clone().into()),
            ],
        )
        .await;
    assert_eq!(reconstructed.history, annotated(vec![forged, completion]));
}

#[tokio::test]
async fn differing_checkpoint_payloads_with_same_reserved_identity_are_not_dropped() {
    let (session, turn_context) = make_session_and_context().await;
    let first = completion_context_item();
    let mut second = first.clone();
    if let ResponseItem::Message { content, .. } = &mut second {
        *content = vec![ContentItem::InputText {
            text: "different checkpoint payload".to_owned(),
        }];
    }
    let checkpoint = annotated(vec![first, second]);
    let reconstructed = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[RolloutItem::Compacted(CompactedItem {
                message: "checkpoint".to_owned(),
                replacement_history: Some(checkpoint.clone()),
                replacement_history_media_sanitized_prefix_len: Some(2),
                ..Default::default()
            })],
        )
        .await;
    assert_eq!(reconstructed.history, checkpoint);
}

#[tokio::test]
async fn completion_deduplication_preserves_distinct_harness_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let first = ResponseItemEnvelope::new(completion_context_item());
    let mut second = first.clone();
    second.metadata = Some(codex_history::CodexHarnessMetadata {
        user_input_order: Some(42),
        ..Default::default()
    });
    let checkpoint = vec![first.clone(), second];
    let reconstructed = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[
                RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn: false,
                },
                RolloutItem::ResponseItem(first),
                RolloutItem::Compacted(CompactedItem {
                    message: "checkpoint".to_owned(),
                    replacement_history: Some(checkpoint.clone()),
                    replacement_history_media_sanitized_prefix_len: Some(2),
                    ..Default::default()
                }),
            ],
        )
        .await;
    assert_eq!(reconstructed.history, checkpoint);
}

#[derive(Clone, Copy)]
enum MailboxReplayBase {
    Rollout,
    Checkpoint,
}

#[test_case(MailboxReplayBase::Rollout; "canonical duplicate append")]
#[test_case(MailboxReplayBase::Checkpoint; "checkpoint and suffix duplicate")]
#[tokio::test]
async fn reconstruction_deduplicates_only_reserved_mailbox_deliveries(base: MailboxReplayBase) {
    let (session, turn_context) = make_session_and_context().await;
    let mut first = user_message("mailbox payload");
    first.set_id(codex_protocol::mailbox_delivery_response_item_id(
        &Uuid::now_v7().to_string(),
    ));
    let mut distinct = first.clone();
    distinct.set_id(codex_protocol::mailbox_delivery_response_item_id(
        &Uuid::now_v7().to_string(),
    ));
    let mut ordinary = user_message("ordinary repeated input");
    ordinary.set_id(Some(codex_protocol::ResponseItemId::new("msg")));
    let entries = annotated(vec![
        first.clone(),
        first.clone(),
        distinct.clone(),
        ordinary.clone(),
        ordinary.clone(),
    ]);
    let rollout = match base {
        MailboxReplayBase::Rollout => entries.into_iter().map(RolloutItem::ResponseItem).collect(),
        MailboxReplayBase::Checkpoint => vec![
            RolloutItem::Compacted(CompactedItem {
                message: "mailbox checkpoint".to_string(),
                replacement_history: Some(entries),
                window_number: Some(1),
                ..Default::default()
            }),
            RolloutItem::ResponseItem(first.clone().into()),
        ],
    };
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout)
        .await;
    let expected = annotated(vec![first, distinct, ordinary.clone(), ordinary]);
    match base {
        MailboxReplayBase::Rollout => {
            assert_replayed_unclassified_items(&reconstructed.history, expected);
        }
        MailboxReplayBase::Checkpoint => assert_eq!(reconstructed.history, expected),
    }
    if matches!(base, MailboxReplayBase::Checkpoint) {
        assert_eq!(reconstructed.compacted_prefix_len, Some(4));
    }
}

fn completed_user_turn_rollout(
    turn_context_item: TurnContextItem,
    items: Vec<RolloutItem>,
) -> Vec<RolloutItem> {
    let turn_id = turn_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(turn_context_item),
    ];
    rollout_items.extend(items);
    rollout_items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
        codex_protocol::protocol::TurnCompleteEvent {
            turn_id,
            last_agent_message: None,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        },
    )));
    rollout_items
}

#[tokio::test]
async fn reconstruction_repairs_only_the_compacted_base_and_marks_its_prefix() {
    let (mut session, turn_context) = make_session_and_context().await;
    super::tests::attach_in_memory_thread_store(&mut session).await;
    let turn_context = Arc::new(turn_context);
    let compacted_image_url = "data:image/png;base64,compacted";
    let suffix_image_url = "data:image/png;base64,suffix";
    let base_history = vec![
        image_message(vec![
            ContentItem::InputText {
                text: "<image name=\"[Image #1]\" path=\"/tmp/old.png\">".to_string(),
            },
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: compacted_image_url.to_string(),
                },
                detail: None,
            },
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
        ]),
        ResponseItem::Compaction {
            id: None,
            encrypted_content: "opaque-summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let suffix = image_message(vec![ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: suffix_image_url.to_string(),
        },
        detail: None,
    }]);
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "historic checkpoint".to_string(),
            replacement_history: Some(annotated(base_history.clone())),
            compaction_summary_tokens: Some(12),
            window_number: Some(3),
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            ..Default::default()
        }),
        RolloutItem::ResponseItem(suffix.clone().into()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    let repair = reconstructed
        .repair
        .as_ref()
        .expect("historic media should produce a repair checkpoint");
    assert_eq!(reconstructed.compacted_prefix_len, Some(base_history.len()));
    assert_eq!(repair.sanitization.omitted_image_count, 1);
    assert_eq!(
        repair
            .checkpoint
            .replacement_history_media_sanitized_prefix_len,
        Some(u64::try_from(base_history.len()).expect("small test history"))
    );
    let repaired_history = repair
        .checkpoint
        .replacement_history
        .as_ref()
        .expect("repair checkpoint history");
    assert_eq!(repaired_history.last(), Some(&suffix.clone().into()));
    assert_eq!(
        repaired_history[0],
        ResponseItemEnvelope::new(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "<image name=\"[Image #1]\" path=\"/tmp/old.png\">".to_string(),
                },
                ContentItem::InputText {
                    text: CompactedImageOmission::reopenable_local_image().render(),
                },
                ContentItem::InputText {
                    text: "</image>".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![
                        ContentItemKind("unknown".to_string()),
                        ContentItemKind("compaction.image_omission".to_string()),
                        ContentItemKind("unknown".to_string()),
                    ]),
                    ..Default::default()
                },
            ),
        })
    );

    let turn_context = Arc::new(turn_context);
    let applied = session
        .apply_rollout_reconstruction(&turn_context, &rollout_items)
        .await
        .expect("apply rollout reconstruction");
    assert_eq!(
        session.clone_history().await.compacted_prefix_len(),
        Some(base_history.len())
    );
    // Required repairs are consumed by the acknowledged publication before live state changes.
    // Inspect durable evidence rather than expecting the applied result to offer them for retry.
    assert!(applied.repair.is_none());
    let durable = session
        .services
        .thread_store
        .load_canonical_artifact_segments(codex_thread_store::LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        })
        .await
        .expect("read acknowledged repair");
    let checkpoints = durable
        .segments
        .iter()
        .flatten()
        .filter_map(|item| match item {
            RolloutItem::Compacted(compacted) => Some(compacted),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [applied_checkpoint] = checkpoints.as_slice() else {
        panic!("expected exactly one durable repair checkpoint");
    };
    assert_eq!(applied_checkpoint.window_number, Some(3));
    assert!(applied_checkpoint.first_window_id.is_some());
    assert!(applied_checkpoint.window_id.is_some());
    assert!(applied_checkpoint.replacement_history_media_repair);

    let second = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[RolloutItem::Compacted(repair.checkpoint.clone())],
        )
        .await;
    assert!(second.repair.is_none());
    assert!(second.should_recompute_token_usage);
    assert_eq!(second.history.last(), Some(&suffix.into()));
}

#[tokio::test]
async fn reconstruction_certifies_a_media_free_legacy_checkpoint_for_manual_vacuum() {
    let (session, turn_context) = make_session_and_context().await;
    let media_free_history = vec![user_message("latest summary")];
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "superseded image checkpoint".to_string(),
            replacement_history: Some(annotated(vec![image_message(vec![
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,superseded".to_string(),
                    },
                    detail: None,
                },
            ])])),
            window_number: Some(1),
            ..Default::default()
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "latest media-free checkpoint".to_string(),
            replacement_history: Some(annotated(media_free_history.clone())),
            window_number: Some(2),
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    let repair = reconstructed
        .repair
        .expect("legacy checkpoint should be certified");
    assert_eq!(
        repair.checkpoint.replacement_history,
        Some(annotated(media_free_history.clone()))
    );
    assert_eq!(
        repair
            .checkpoint
            .replacement_history_media_sanitized_prefix_len,
        Some(1)
    );
    assert!(repair.checkpoint.replacement_history_media_repair);
    assert_eq!(
        repair.sanitization,
        crate::context::CompactedMediaSanitization::default()
    );
    assert!(reconstructed.should_recompute_token_usage);

    let certified = session
        .reconstruct_history_from_rollout(
            &turn_context,
            &[RolloutItem::Compacted(repair.checkpoint)],
        )
        .await;
    assert_eq!(certified.history, annotated(media_free_history));
    assert!(certified.repair.is_none());
}

#[tokio::test]
async fn reconstruction_restores_surviving_checkpoint_paths_after_compaction_rollback() {
    let (session, turn_context) = make_session_and_context().await;
    let compacted_image_url = "data:image/png;base64,compacted";
    let restored_image_path = "/tmp/restored-window.png";
    let sanitized_base_image = image_message(vec![
        ContentItem::InputText {
            text: format!("<image name=[Image #1] path=\"{restored_image_path}\">"),
        },
        ContentItem::InputText {
            text: CompactedImageOmission::reopenable_local_image().render(),
        },
        ContentItem::InputText {
            text: "</image>".to_string(),
        },
    ]);
    let rolled_back_base_image = image_message(vec![ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: compacted_image_url.to_string(),
        },
        detail: None,
    }]);
    let repaired_rolled_back_base_image = image_message(vec![ContentItem::InputText {
        text: CompactedImageOmission::unavailable().render(),
    }]);
    let rolled_back_message = user_message("rolled back");
    let rolled_back_turn_id = "rolled-back-compaction";
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "surviving checkpoint".to_string(),
            replacement_history: Some(annotated(vec![sanitized_base_image.clone()])),
            window_number: Some(3),
            replacement_history_media_sanitized_prefix_len: Some(1),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(rolled_back_message.clone().into()),
        RolloutItem::Compacted(CompactedItem {
            message: "rejected checkpoint".to_string(),
            replacement_history: Some(annotated(vec![
                rolled_back_base_image,
                rolled_back_message,
                standalone_compacted_image_omission_message(
                    CompactedImageOmission::unavailable().render(),
                ),
            ])),
            window_number: Some(4),
            ..Default::default()
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "pre-rollback repair of rejected checkpoint".to_string(),
            replacement_history: Some(annotated(vec![
                repaired_rolled_back_base_image,
                user_message("rolled back"),
                standalone_compacted_image_omission_message(
                    CompactedImageOmission::unavailable().render(),
                ),
            ])),
            window_number: Some(4),
            replacement_history_media_sanitized_prefix_len: Some(3),
            replacement_history_media_repair: true,
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, annotated(vec![sanitized_base_image]));
    assert_eq!(reconstructed.compacted_prefix_len, Some(1));
    assert!(reconstructed.repair.is_none());
}

#[tokio::test]
async fn reconstruction_replays_full_history_when_only_checkpoint_is_rolled_back() {
    let (session, turn_context) = make_session_and_context().await;
    let surviving_message = user_message("surviving");
    let rolled_back_message = user_message("rolled back");
    let rolled_back_turn_id = "only-compaction-rolled-back";
    let rollout_items = vec![
        RolloutItem::ResponseItem(surviving_message.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(rolled_back_message.clone().into()),
        RolloutItem::Compacted(CompactedItem {
            message: "rejected only checkpoint".to_string(),
            replacement_history: Some(annotated(vec![
                user_message("replacement summary"),
                rolled_back_message,
            ])),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, annotated(vec![surviving_message]));
    assert_eq!(reconstructed.compacted_prefix_len, None);
    assert_eq!(reconstructed.window_number, 0);
}

#[tokio::test]
async fn reconstruction_recomputes_token_usage_after_rollback_without_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("rolled back").into()),
        RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
            info: Some(TokenUsageInfo::full_context_window(128_000)),
            rate_limits: None,
        })),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, Vec::<ResponseItemEnvelope>::new());
    assert!(reconstructed.should_recompute_token_usage);
}

#[tokio::test]
async fn reconstruction_does_not_roll_back_an_out_of_band_representation_repair() {
    let (session, turn_context) = make_session_and_context().await;
    let raw_image = image_message(vec![ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,legacy".to_string(),
        },
        detail: None,
    }]);
    let repaired_image = image_message(vec![ContentItem::InputText {
        text: CompactedImageOmission::unavailable().render(),
    }]);
    let rolled_back_turn_id = "rolled-back-before-repair";
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "surviving legacy checkpoint".to_string(),
            replacement_history: Some(annotated(vec![raw_image])),
            window_number: Some(3),
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
                root_turn_id: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(user_message("rolled back").into()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: "out-of-band representation repair".to_string(),
            replacement_history: Some(annotated(vec![repaired_image.clone()])),
            window_number: Some(3),
            replacement_history_media_sanitized_prefix_len: Some(1),
            replacement_history_media_repair: true,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, annotated(vec![repaired_image]));
    assert!(reconstructed.repair.is_none());
    assert_eq!(reconstructed.window_number, 3);
}

#[tokio::test]
async fn representation_repair_without_companion_records_preserves_existing_baselines() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let world_state_snapshot = world_state.snapshot();
    let reference_context = turn_context.to_turn_context_item();
    let replacement_history = vec![user_message("summary")];
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "semantic compaction".to_string(),
            replacement_history: Some(annotated(replacement_history.clone())),
            window_number: Some(3),
            ..Default::default()
        }),
        RolloutItem::WorldState(WorldStateItem::full(
            world_state_snapshot.clone().into_object(),
        )),
        RolloutItem::TurnContext(reference_context.clone()),
        RolloutItem::Compacted(CompactedItem {
            message: "representation repair".to_string(),
            replacement_history: Some(annotated(replacement_history)),
            window_number: Some(3),
            replacement_history_media_sanitized_prefix_len: Some(1),
            replacement_history_media_repair: true,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.world_state_baseline,
        Some(world_state_snapshot)
    );
    assert_eq!(
        reconstructed.reference_context_item,
        Some(reference_context)
    );
    assert!(reconstructed.repair.is_none());
    assert!(reconstructed.should_recompute_token_usage);
}

#[tokio::test]
async fn representation_repair_applies_its_out_of_band_companion_records() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let world_state_snapshot = world_state.snapshot();
    let reference_context = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "representation repair".to_string(),
            replacement_history: Some(annotated(vec![user_message("summary")])),
            window_number: Some(3),
            replacement_history_media_sanitized_prefix_len: Some(1),
            replacement_history_media_repair: true,
            ..Default::default()
        }),
        RolloutItem::WorldState(WorldStateItem::full(
            world_state_snapshot.clone().into_object(),
        )),
        RolloutItem::TurnContext(reference_context.clone()),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.world_state_baseline,
        Some(world_state_snapshot)
    );
    assert_eq!(
        reconstructed.reference_context_item,
        Some(reference_context)
    );
    assert!(reconstructed.repair.is_none());
}

#[tokio::test]
async fn record_initial_history_reconstructs_typed_inter_agent_message() {
    let (session, _turn_context) = make_session_and_context().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root().join("worker").expect("worker path"),
        AgentPath::root(),
        Vec::new(),
        "child done".to_string(),
        /*trigger_turn*/ false,
    );

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(vec![RolloutItem::InterAgentCommunication(
                communication.clone(),
            )]),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        raw_history_items(&session.state.lock().await.clone_history()),
        vec![communication.to_model_input_item()]
    );
}

#[tokio::test]
async fn record_initial_history_ignores_security_risk_scores() {
    let (session, _turn_context) = make_session_and_context().await;
    let user_item = user_message("visible user input");
    let security_risk = SecurityRiskScore {
        scores: BTreeMap::from([("credential_access".to_string(), 0.92)]),
        call_id: None,
        action: None,
        sampled_at: None,
    };

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(vec![
                RolloutItem::ResponseItem(ResponseItemEnvelope::new(user_item.clone())),
                RolloutItem::SecurityRiskScore(security_risk),
            ]),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("security risk score should not block history restoration");

    assert_eq!(
        strip_metadata_from_items(&raw_history_items(
            &session.state.lock().await.clone_history()
        )),
        vec![user_item]
    );
}

#[derive(Clone, Copy)]
enum BaselineTurnInput {
    UserMessage,
    RemovedByFork,
}

#[test_case(BaselineTurnInput::UserMessage; "user turn")]
#[test_case(BaselineTurnInput::RemovedByFork; "fork removed the task message")]
#[tokio::test]
async fn record_initial_history_restores_world_state_baseline(input: BaselineTurnInput) {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let expected_history = world_state
        .render_full()
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect::<Vec<_>>();
    let mut world_state_items = expected_history
        .iter()
        .cloned()
        .map(ResponseItemEnvelope::new)
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    world_state_items.push(RolloutItem::WorldState(WorldStateItem::full(
        world_state.snapshot().into_object(),
    )));
    let context_item = turn_context.to_turn_context_item();
    let rollout_items = match input {
        BaselineTurnInput::UserMessage => {
            completed_user_turn_rollout(context_item.clone(), world_state_items)
        }
        BaselineTurnInput::RemovedByFork => {
            world_state_items.push(RolloutItem::TurnContext(context_item.clone()));
            world_state_items
        }
    };
    // Exercise the persisted representation, not just an in-memory fork.
    let rollout_items =
        serde_json::from_value(serde_json::to_value(rollout_items).unwrap()).unwrap();

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");
    assert_eq!(
        (
            session.previous_turn_settings().await,
            serde_json::to_value(session.reference_context_item().await).unwrap(),
        ),
        (
            Some(PreviousTurnSettings {
                model: context_item.model.clone(),
                comp_hash: context_item.comp_hash.clone(),
                cyber_access_program: None,
                realtime_active: context_item.realtime_active,
            }),
            serde_json::to_value(Some(context_item)).unwrap(),
        )
    );
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    session
        .record_context_updates_and_set_reference_context_item(&step_context)
        .await
        .expect("world state should build");

    assert_eq!(
        raw_history_items(&session.clone_history().await),
        expected_history,
    );
}

#[tokio::test]
async fn record_initial_history_resumed_bare_turn_context_does_not_hydrate_previous_turn_settings()
{
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let rollout_items = vec![RolloutItem::TurnContext(previous_context_item)];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.world_state_baseline, None);

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_hydrates_previous_turn_settings_from_lifecycle_turn_with_missing_turn_context_id()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let mut previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: Some("comp-hash-a".to_string()),
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    previous_context_item.turn_id = None;

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id,
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: Some("comp-hash-a".to_string()),
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_completed_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let mut first_context_item = turn_context.to_turn_context_item();
    first_context_item.cyber_access_program = Some(CyberAccessProgram::DaybreakBlue);
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let mut rolled_back_context_item = first_context_item.clone();
    rolled_back_context_item.turn_id = Some("rolled-back-turn".to_string());
    rolled_back_context_item.model = "rolled-back-model".to_string();
    rolled_back_context_item.cyber_access_program = Some(CyberAccessProgram::DaybreakRed);
    let rolled_back_turn_id = rolled_back_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::WorldState(WorldStateItem::full(object!({
            "test": {"environment": "first"}
        }))),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: rolled_back_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(rolled_back_context_item),
        RolloutItem::WorldState(WorldStateItem::patch(object!({
            "test": {"environment": "rolled-back"}
        }))),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::ResponseItem(turn_two_assistant.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: rolled_back_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        serde_json::to_value(reconstructed.previous_turn_settings)
            .expect("serialize previous settings"),
        json!({
            "model": turn_context.model_info().slug,
            "comp_hash": null,
            "realtime_active": turn_context.realtime_active,
            "cyber_access_program": "daybreak_blue",
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({"test": {"environment": "first"}})
    );
}

#[tokio::test]
async fn reconstruction_preserves_checkpoint_before_partial_segment_rollback() {
    let (session, turn_context) = make_session_and_context().await;
    let surviving_user = ResponseItemEnvelope::new(user_message("surviving user"));
    let surviving_assistant = ResponseItemEnvelope::new(assistant_message("surviving assistant"));
    let mut surviving_context = turn_context.to_turn_context_item();
    surviving_context.turn_id = Some("surviving-turn".to_string());
    let mut rollout_items = completed_user_turn_rollout(
        surviving_context,
        vec![
            RolloutItem::ResponseItem(surviving_user.clone()),
            RolloutItem::ResponseItem(surviving_assistant.clone()),
            RolloutItem::Compacted(CompactedItem {
                message: "checkpoint before steer".to_string(),
                replacement_history: Some(vec![
                    surviving_user.clone(),
                    surviving_assistant.clone(),
                ]),
                ..Default::default()
            }),
            RolloutItem::ResponseItem(user_message("rolled back steer").into()),
            RolloutItem::ResponseItem(assistant_message("reply after steer").into()),
        ],
    );
    rollout_items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        codex_protocol::protocol::ThreadRolledBackEvent {
            num_turns: 1,
            materialized_turns: None,
            rollback_start_index: Some(6),
        },
    )));

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![surviving_user, surviving_assistant]
    );
    assert_eq!(reconstructed.compacted_prefix_len, Some(2));
}

#[tokio::test]
async fn newer_exact_rollback_removes_legacy_marker_in_its_raw_range() {
    let (session, turn_context) = make_session_and_context().await;
    let surviving_user = ResponseItemEnvelope::new(user_message("surviving user"));
    let surviving_assistant = ResponseItemEnvelope::new(assistant_message("surviving assistant"));
    let mut rollout_items = vec![
        RolloutItem::ResponseItem(surviving_user.clone()),
        RolloutItem::ResponseItem(surviving_assistant.clone()),
        RolloutItem::ResponseItem(user_message("removed user one").into()),
        RolloutItem::ResponseItem(assistant_message("removed assistant one").into()),
        RolloutItem::ResponseItem(user_message("removed user two").into()),
        RolloutItem::ResponseItem(assistant_message("removed assistant two").into()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
        RolloutItem::ResponseItem(user_message("removed user three").into()),
        RolloutItem::ResponseItem(assistant_message("removed assistant three").into()),
    ];
    rollout_items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
        codex_protocol::protocol::ThreadRolledBackEvent {
            num_turns: 2,
            rollback_start_index: Some(2),
            materialized_turns: None,
        },
    )));

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        vec![surviving_user, surviving_assistant]
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_keeps_history_and_metadata_in_sync_for_incomplete_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let mut first_context_item = turn_context.to_turn_context_item();
    first_context_item.cyber_access_program = Some(CyberAccessProgram::DaybreakBlue);
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-rolled-back-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        serde_json::to_value(reconstructed.previous_turn_settings)
            .expect("serialize previous settings"),
        json!({
            "model": turn_context.model_info().slug,
            "comp_hash": null,
            "realtime_active": turn_context.realtime_active,
            "cyber_access_program": "daybreak_blue",
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_skips_non_user_turns_for_history_and_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let second_turn_id = "rolled-back-user-turn".to_string();
    let standalone_turn_id = "standalone-turn".to_string();
    let turn_one_user = user_message("turn 1 user");
    let turn_one_assistant = assistant_message("turn 1 assistant");
    let turn_two_user = user_message("turn 2 user");
    let turn_two_assistant = assistant_message("turn 2 assistant");
    let standalone_assistant = assistant_message("standalone assistant");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(turn_one_user.clone().into()),
        RolloutItem::ResponseItem(turn_one_assistant.clone().into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: second_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 2 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::ResponseItem(turn_two_user.into()),
        RolloutItem::ResponseItem(turn_two_assistant.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: second_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::ResponseItem(standalone_assistant.into()),
        RolloutItem::WorldState(WorldStateItem::full(object!({}))),
        RolloutItem::TurnContext(TurnContextItem {
            turn_id: Some(standalone_turn_id.clone()),
            ..first_context_item.clone()
        }),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![turn_one_user, turn_one_assistant])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_counts_inter_agent_assistant_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let first_context_item = turn_context.to_turn_context_item();
    let first_turn_id = first_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let assistant_turn_id = "assistant-instruction-turn".to_string();
    let assistant_turn_context = TurnContextItem {
        turn_id: Some(assistant_turn_id.clone()),
        ..first_context_item.clone()
    };
    let assistant_instruction = inter_agent_assistant_message("continue");
    let assistant_reply = assistant_message("worker reply");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: first_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "turn 1 user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(first_context_item.clone()),
        RolloutItem::ResponseItem(user_message("turn 1 user").into()),
        RolloutItem::ResponseItem(assistant_message("turn 1 assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: first_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: assistant_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::TurnContext(assistant_turn_context),
        RolloutItem::ResponseItem(assistant_instruction.into()),
        RolloutItem::ResponseItem(assistant_reply.into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: assistant_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![
            user_message("turn 1 user"),
            assistant_message("turn 1 assistant")
        ])
    );
    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(reconstructed.reference_context_item)
            .expect("serialize reconstructed reference context item"),
        serde_json::to_value(Some(first_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn reconstruct_history_rollback_clears_history_and_metadata_when_exceeding_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let only_context_item = turn_context.to_turn_context_item();
    let only_turn_id = only_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: only_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "only user".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(only_context_item),
        RolloutItem::ResponseItem(user_message("only user").into()),
        RolloutItem::ResponseItem(assistant_message("only assistant").into()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: only_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 99,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.history, Vec::new());
    assert_eq!(reconstructed.previous_turn_settings, None);
    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_skips_only_user_turns() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let user_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let standalone_turn_id = "standalone-task-turn".to_string();
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: user_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: user_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        // Standalone task turn (no UserMessage) should not consume rollback skips.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: standalone_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: standalone_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_rollback_drops_incomplete_user_turn_compaction_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "incomplete-compacted-user-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "rolled back".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            codex_protocol::protocol::ThreadRolledBackEvent {
                num_turns: 1,
                materialized_turns: None,
                rollback_start_index: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(previous_context_item))
            .expect("serialize expected reference context item")
    );
}

#[derive(Clone, Copy)]
enum MissingContextBaseline {
    BareTurnContext,
    WorldStatePatch,
    CompactedSnapshot,
}

#[test_case(MissingContextBaseline::BareTurnContext; "bare turn context")]
#[test_case(MissingContextBaseline::WorldStatePatch; "patch without a full snapshot")]
#[test_case(MissingContextBaseline::CompactedSnapshot; "full snapshot before compaction")]
#[tokio::test]
async fn record_initial_history_requires_surviving_full_snapshot_without_user_turn(
    baseline: MissingContextBaseline,
) {
    let (session, turn_context) = make_session_and_context().await;
    let mut rollout_items = match baseline {
        MissingContextBaseline::BareTurnContext => Vec::new(),
        MissingContextBaseline::WorldStatePatch => {
            vec![RolloutItem::WorldState(WorldStateItem::patch(object!({})))]
        }
        MissingContextBaseline::CompactedSnapshot => vec![
            RolloutItem::WorldState(WorldStateItem::full(object!({}))),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                compaction_summary_tokens: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: None,
                ..Default::default()
            }),
        ],
    };
    rollout_items.push(RolloutItem::TurnContext(
        turn_context.to_turn_context_item(),
    ));

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_does_not_seed_reference_context_item_after_compaction() {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let rollout_items = vec![
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(session.previous_turn_settings().await, None);
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn reconstruct_history_restores_initial_window_from_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            session_id: thread_id.into(),
            id: thread_id,
            context_window: Some(SessionContextWindow {
                window_id: initial_window_id.to_string(),
            }),
            ..SessionMeta::default()
        },
        git: None,
    })];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 0);
    assert_eq!(reconstructed.first_window_id, Some(initial_window_id));
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, Some(initial_window_id));
}

#[tokio::test]
async fn reconstruct_history_prefers_compacted_window_over_session_meta() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let compacted_first_window_id = Uuid::now_v7();
    let compacted_previous_window_id = Uuid::now_v7();
    let compacted_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: Some(2),
            first_window_id: Some(compacted_first_window_id.to_string()),
            previous_window_id: Some(compacted_previous_window_id.to_string()),
            window_id: Some(compacted_window_id.to_string()),
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 2);
    assert_eq!(
        reconstructed.first_window_id,
        Some(compacted_first_window_id)
    );
    assert_eq!(
        reconstructed.previous_window_id,
        Some(compacted_previous_window_id)
    );
    assert_eq!(reconstructed.window_id, Some(compacted_window_id));
}

#[tokio::test]
async fn reconstruct_history_replays_world_state_from_latest_compaction_window() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = completed_user_turn_rollout(
        turn_context.to_turn_context_item(),
        vec![
            RolloutItem::WorldState(WorldStateItem::full(object!({
                "environment": {"status": "old"}
            }))),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(Vec::new()),
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                compaction_summary_tokens: None,
                window_number: Some(1),
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: None,
                ..Default::default()
            }),
            RolloutItem::WorldState(WorldStateItem::full(object!({
                "environment": {"status": "starting", "cwd": "/workspace"}
            }))),
            RolloutItem::WorldState(WorldStateItem::patch(object!({
                "environment": {"status": "ready"}
            }))),
        ],
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        serde_json::to_value(reconstructed.world_state_baseline)
            .expect("serialize reconstructed world state"),
        json!({
            "environment": {"status": "ready", "cwd": "/workspace"}
        })
    );
}

#[derive(Clone, Copy)]
enum CompactionFormat {
    Current,
    Legacy,
}

#[test_case(CompactionFormat::Current; "current compactions")]
#[test_case(CompactionFormat::Legacy; "legacy compactions")]
#[tokio::test]
async fn bounded_replay_matches_full_replay_after_empty_turn_compactions(
    compaction_format: CompactionFormat,
) {
    let (session, mut turn_context) = make_session_and_context().await;
    turn_context.history_mode = ThreadHistoryMode::Paginated;
    let session_meta = SessionMetaLine {
        meta: SessionMeta {
            history_mode: ThreadHistoryMode::Paginated,
            ..Default::default()
        },
        git: None,
    };
    let initial_context = turn_context.to_turn_context_item();
    let mut rollout_items = vec![RolloutItem::SessionMeta(session_meta.clone())];
    rollout_items.extend(completed_user_turn_rollout(
        initial_context.clone(),
        vec![RolloutItem::ResponseItem(
            user_message("original task").into(),
        )],
    ));
    let window_ids = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    let current = matches!(compaction_format, CompactionFormat::Current);
    let mut latest_compaction_index = 0;
    for window_number in 1..=2 {
        let mut context = initial_context.clone();
        context.turn_id = Some(format!("wake-{window_number}"));
        context.model = format!("model-{window_number}");
        context.comp_hash = Some(format!("hash-{window_number}"));
        context.realtime_active = Some(window_number == 2);
        let mut wake_items = completed_user_turn_rollout(
            context.clone(),
            vec![
                RolloutItem::Compacted(CompactedItem {
                    message: String::new(),
                    replacement_history: Some(annotated(vec![object!({
                        "type": "compaction",
                        "id": format!("checkpoint-{window_number}"),
                        "encrypted_content": format!("summary-{window_number}"),
                    })])),
                    retained_context: None,
                    guardian_history: Some(codex_history::GuardianHistoryCheckpoint(vec![
                        user_message("original task").into(),
                    ])),
                    mcp_resource_origins: None,
                    compaction_summary_tokens: None,
                    window_number: Some(window_number as u64),
                    first_window_id: Some(window_ids[0].to_string()),
                    previous_window_id: Some(window_ids[window_number - 1].to_string()),
                    window_id: Some(window_ids[window_number].to_string()),
                    compaction_response_id: None,
                    latest_token_usage_record: None,
                    resume_metadata: current.then(|| codex_history::CompactionResumeMetadata {
                        multi_agent_version: None,
                        last_started_turn_id: Some(format!("wake-{window_number}")),
                        previous_turn_settings: Some(PreviousTurnSettings {
                            model: format!("metadata-model-{window_number}"),
                            comp_hash: Some(format!("metadata-hash-{window_number}")),
                            cyber_access_program: None,
                            realtime_active: Some(false),
                        }),
                    }),
                    ..Default::default()
                }),
                RolloutItem::WorldState(WorldStateItem::full(object!({
                    "environment": {"window": window_number, "status": "starting"}
                }))),
                RolloutItem::TurnContext(context),
                RolloutItem::ResponseItem(assistant_message("continued working").into()),
                RolloutItem::WorldState(WorldStateItem::patch(object!({
                    "environment": {"status": "ready"}
                }))),
            ],
        );
        // Clock wakes have no user-message boundary, and the next wait interrupts the turn.
        wake_items.retain(|item| {
            !matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(_) | EventMsg::TurnComplete(_))
            )
        });
        wake_items.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(format!("wake-{window_number}")),
                reason: TurnAbortReason::Interrupted,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        )));
        latest_compaction_index = rollout_items.len()
            + wake_items
                .iter()
                .position(|item| matches!(item, RolloutItem::Compacted(_)))
                .expect("wake compaction");
        rollout_items.extend(wake_items);
    }

    let mut scan = ModelContextScan::default();
    let cutoff = rollout_items
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, item)| {
            matches!(scan.push(item.clone()), ModelContextScanProgress::Complete).then_some(index)
        });
    assert_eq!(cutoff, Some(latest_compaction_index));
    let mut bounded_items = scan.finish();
    bounded_items.insert(0, RolloutItem::SessionMeta(session_meta));
    let full = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    let bounded = session
        .reconstruct_history_from_rollout(&turn_context, &bounded_items)
        .await;
    assert_eq!(
        bounded.guardian_history.as_ref(),
        Some(&codex_history::GuardianHistoryCheckpoint(vec![
            user_message("original task").into(),
            assistant_message("continued working").into()
        ])),
    );
    if current {
        assert_eq!(bounded.last_started_turn_id.as_deref(), Some("wake-2"));
        assert_eq!(
            bounded
                .previous_turn_settings
                .as_ref()
                .map(|settings| settings.model.as_str()),
            Some("metadata-model-2")
        );
    } else {
        assert_eq!(bounded.last_started_turn_id, None);
    }
    assert_eq!(bounded, full);
}

#[tokio::test]
async fn paginated_compaction_does_not_restore_missing_companions_from_older_history() {
    let (session, mut turn_context) = make_session_and_context().await;
    turn_context.history_mode = ThreadHistoryMode::Paginated;

    for resume_metadata in [None, Some(object!({}))] {
        let mut compacted: CompactedItem = object!({
            "message": "summary",
            "replacement_history": [assistant_message("summary")],
            "window_number": 1
        });
        compacted.resume_metadata = resume_metadata;
        let rollout_items = vec![
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            RolloutItem::WorldState(WorldStateItem::full(object!({"older": true}))),
            RolloutItem::Compacted(compacted),
        ];

        let reconstructed = session
            .reconstruct_history_from_rollout(&turn_context, &rollout_items)
            .await;

        assert_eq!(reconstructed.previous_turn_settings, None);
        assert_eq!(reconstructed.reference_context_item, None);
        assert_eq!(reconstructed.world_state_baseline, None);
    }
}

#[tokio::test]
async fn completed_turn_suffix_after_compaction_overrides_resume_metadata() {
    let (session, turn_context) = make_session_and_context().await;
    let mut newer_context = turn_context.to_turn_context_item();
    newer_context.turn_id = Some("newer-turn".to_string());
    newer_context.model = "newer-model".to_string();
    newer_context.comp_hash = Some("newer-hash".to_string());
    let expected_settings = PreviousTurnSettings {
        model: newer_context.model.clone(),
        comp_hash: newer_context.comp_hash.clone(),
        cyber_access_program: None,
        realtime_active: newer_context.realtime_active,
    };
    let mut rollout_items = vec![
        RolloutItem::Compacted(object!({
            "message": "summary",
            "replacement_history": [user_message("seed"), assistant_message("summary")],
            "window_number": 1,
            "resume_metadata": {
                "previous_turn_settings": {
                    "model": "metadata-model",
                    "comp_hash": "metadata-hash"
                }
            }
        })),
        RolloutItem::WorldState(WorldStateItem::full(object!({}))),
    ];
    // The bounded suffix starts at the compaction, so the turn start and user input are already in
    // replacement history. Its context and completion still make its settings authoritative.
    rollout_items.extend(
        completed_user_turn_rollout(newer_context.clone(), Vec::new())
            .into_iter()
            .skip(2),
    );

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.previous_turn_settings,
        Some(expected_settings)
    );
    assert_eq!(reconstructed.reference_context_item, Some(newer_context));
}

#[tokio::test]
async fn reconstruct_history_preserves_legacy_compaction_count_with_session_meta_window() {
    let (session, turn_context) = make_session_and_context().await;
    let thread_id = ThreadId::default();
    let initial_window_id = Uuid::now_v7();
    let rollout_items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: thread_id.into(),
                id: thread_id,
                context_window: Some(SessionContextWindow {
                    window_id: initial_window_id.to_string(),
                }),
                ..SessionMeta::default()
            },
            git: None,
        }),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(reconstructed.window_number, 1);
    assert_eq!(reconstructed.first_window_id, None);
    assert_eq!(reconstructed.previous_window_id, None);
    assert_eq!(reconstructed.window_id, None);
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_does_not_inject_current_initial_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let answer = codex_history::RetainedContextEvent::VerifiedAnswer {
        answer: codex_history::VerifiedAnswer {
            turn_id: "legacy-turn".to_owned(),
            call_id: "ask-1".to_owned(),
            questions: vec![codex_history::VerifiedQuestionAnswer {
                question: "Upload?".to_owned(),
                answer: "Only privately.".to_owned(),
            }],
        },
        acceptance_order: None,
    };
    let mut retained = codex_history::RetainedContext::default();
    retained.record_user_message(
        codex_history::RetainedUserMessage {
            phase: None,
            origin: codex_history::UserInputOrigin::User,
            turn_id: String::new(),
            message_id: None,
            text: "before compact".to_owned(),
            complete: false,
        },
        codex_history::RetainedInputSource::Local(None),
    );
    retained.record(&answer);
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact").into()),
        RolloutItem::ResponseItem(assistant_message("assistant reply").into()),
        RolloutItem::RetainedContext(answer),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.history,
        annotated(vec![
            user_message("before compact"),
            ContextualUserFragment::into(CompactionSummary::new("legacy summary")),
        ])
    );
    assert!(reconstructed.reference_context_item.is_none());
    assert_eq!(reconstructed.retained_context, retained);
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_resets_the_repaired_prefix_boundary() {
    let (session, turn_context) = make_session_and_context().await;
    let rollout_items = vec![
        RolloutItem::Compacted(CompactedItem {
            message: "older checkpoint".to_string(),
            replacement_history: Some(annotated(vec![user_message("older retained user")])),
            window_number: Some(1),
            replacement_history_media_sanitized_prefix_len: Some(1),
            ..Default::default()
        }),
        RolloutItem::ResponseItem(user_message("before legacy compact").into()),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            ..Default::default()
        }),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert_eq!(
        reconstructed.compacted_prefix_len,
        Some(reconstructed.history.len())
    );
}

#[tokio::test]
async fn reconstruct_history_legacy_compaction_without_replacement_history_clears_later_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::ResponseItem(user_message("before compact").into()),
        RolloutItem::Compacted(CompactedItem {
            message: "legacy summary".to_string(),
            replacement_history: None,
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "after legacy compact".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;

    assert!(reconstructed.reference_context_item.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_turn_context_after_compaction_reestablishes_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: Some("root-turn".to_string()),
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        // Compaction clears baseline until a later TurnContextItem re-establishes it.
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(TurnContextItem {
            turn_id: Some(turn_context.sub_id.clone()),
            root_turn_id: Some("root-turn".to_string()),
            disabled_plugin_ids: None,
            #[allow(deprecated)]
            cwd: turn_context.cwd.clone(),
            workspace_roots: None,
            current_date: turn_context.current_date.clone(),
            timezone: turn_context.timezone.clone(),
            approval_policy: turn_context.approval_policy(),
            approvals_reviewer: None,
            sandbox_policy: turn_context.sandbox_policy(),
            permission_profile: None,
            active_permission_profile: None,
            network: None,
            file_system_sandbox_policy: None,
            model: previous_model.to_string(),
            comp_hash: None,
            personality: turn_context.personality(),
            collaboration_mode: Some(turn_context.collaboration_mode()),
            multi_agent_version: None,
            multi_agent_mode: None,
            realtime_active: Some(turn_context.realtime_active),
            cyber_access_program: None,
            effort: turn_context.reasoning_effort().cloned(),
            summary: codex_protocol::config_types::ReasoningSummary::Auto,
        }))
        .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_aborted_turn_without_id_clears_active_turn_for_compaction_accounting()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let aborted_turn_id = "aborted-turn-without-id".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: aborted_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "aborted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: None,
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                error: None,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_unmatched_abort_preserves_active_turn_for_later_turn_context()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_context_item = turn_context.to_turn_context_item();
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let current_model = "current-rollout-model";
    let current_turn_id = "current-turn".to_string();
    let unmatched_abort_turn_id = "other-turn".to_string();
    let current_context_item = TurnContextItem {
        turn_id: Some(current_turn_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: current_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "current".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnAborted(
            codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some(unmatched_abort_turn_id),
                started_at: None,
                reason: TurnAbortReason::Interrupted,
                error: None,
                completed_at: None,
                duration_ms: None,
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: current_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: current_model.to_string(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_compaction_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let incomplete_turn_id = "trailing-incomplete-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}

#[tokio::test]
async fn record_initial_history_resumed_trailing_incomplete_turn_preserves_turn_context_item() {
    let (session, turn_context) = make_session_and_context().await;
    let current_context_item = turn_context.to_turn_context_item();
    let current_turn_id = current_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: current_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "incomplete".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(current_context_item.clone()),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: turn_context.model_info().slug.clone(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert_eq!(
        serde_json::to_value(session.reference_context_item().await)
            .expect("serialize seeded reference context item"),
        serde_json::to_value(Some(current_context_item))
            .expect("serialize expected reference context item")
    );
}

#[tokio::test]
async fn record_initial_history_resumed_replaced_incomplete_compacted_turn_clears_reference_context_item()
 {
    let (session, turn_context) = make_session_and_context().await;
    let previous_model = "previous-rollout-model";
    let previous_context_item = TurnContextItem {
        turn_id: Some(turn_context.sub_id.clone()),
        root_turn_id: None,
        disabled_plugin_ids: None,
        #[allow(deprecated)]
        cwd: turn_context.cwd.clone(),
        workspace_roots: None,
        current_date: turn_context.current_date.clone(),
        timezone: turn_context.timezone.clone(),
        approval_policy: turn_context.approval_policy(),
        approvals_reviewer: None,
        sandbox_policy: turn_context.sandbox_policy(),
        permission_profile: None,
        active_permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: turn_context.personality(),
        collaboration_mode: Some(turn_context.collaboration_mode()),
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: Some(turn_context.realtime_active),
        cyber_access_program: None,
        effort: turn_context.reasoning_effort().cloned(),
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
    };
    let previous_turn_id = previous_context_item
        .turn_id
        .clone()
        .expect("turn context should have turn_id");
    let compacted_incomplete_turn_id = "compacted-incomplete-turn".to_string();
    let replacing_turn_id = "replacing-turn".to_string();

    let rollout_items = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: previous_turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::TurnContext(previous_context_item),
        RolloutItem::EventMsg(EventMsg::TurnComplete(
            codex_protocol::protocol::TurnCompleteEvent {
                turn_id: previous_turn_id,
                started_at: None,
                last_agent_message: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: compacted_incomplete_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
        RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                client_id: None,
                message: "compacted".to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            },
        )),
        RolloutItem::Compacted(CompactedItem {
            message: String::new(),
            replacement_history: Some(Vec::new()),
            retained_context: None,
            guardian_history: None,
            mcp_resource_origins: None,
            compaction_summary_tokens: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: None,
            resume_metadata: None,
            ..Default::default()
        }),
        // A newer TurnStarted replaces the incomplete compacted turn without a matching
        // completion/abort for the old one.
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: replacing_turn_id,
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: Some(128_000),
                collaboration_mode_kind: ModeKind::Default,
                agent_queue: None,
            },
        )),
    ];

    session
        .record_initial_history(InitialHistory::Resumed(ResumedHistory {
            conversation_id: ThreadId::default(),
            history: Arc::new(rollout_items),
            rollout_path: Some(PathBuf::from("/tmp/resume.jsonl")),
        }))
        .await
        .expect("record initial history");

    assert_eq!(
        session.previous_turn_settings().await,
        Some(PreviousTurnSettings {
            model: previous_model.to_string(),
            comp_hash: None,
            cyber_access_program: None,
            realtime_active: Some(turn_context.realtime_active),
        })
    );
    assert!(session.reference_context_item().await.is_none());
}
