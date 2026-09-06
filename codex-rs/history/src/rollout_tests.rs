use super::*;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserAgentControlAction;
use codex_protocol::items::UserAgentControlItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_item;
use pretty_assertions::assert_eq;

fn message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn marker(start: u64) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 0,
        materialized_turns: None,
        rollback_start_index: Some(start),
    }))
}

fn started(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: id.into(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
        agent_queue: None,
    }))
}

fn completed(id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: id.into(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

fn completion_context_message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: Some(new_sub_agent_completion_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn user_agent_task_context_message(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: Some(new_user_agent_task_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn persistent_agent_reply_route() -> RolloutItem {
    let agent_id = ThreadId::new();
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!(
                    "<agent_reply_route>\n{{\"agent_id\":\"{agent_id}\",\"send_input\":\"allowed_until_disabled\"}}\n</agent_reply_route>"
                ),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: Some(
                InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind(
                        PERSISTENT_AGENT_REPLY_ROUTE_CONTENT_KIND.to_string(),
                    )]),
                    ..Default::default()
                },
            ),
        }
        .into(),
    )
}

fn inter_agent_completion_context(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: Some(new_sub_agent_completion_context_response_item_id()),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: text.to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn completion_event(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::AgentMessage(
            sub_agent_completion_item(
                "/root/worker",
                &AgentStatus::Completed(Some("done".to_string())),
            )
            .expect("terminal status"),
        ),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

fn user_agent_control_event(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: crate::ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::UserAgentControl(UserAgentControlItem::succeeded(
            UserAgentControlAction::Prompt,
        )),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

#[test]
fn user_agent_control_from_before_target_turn_flags_defaults_new_fields() {
    let expected = UserAgentControlItem::succeeded(UserAgentControlAction::Prompt);
    let mut value = serde_json::to_value(&expected).expect("serialize user agent control");
    let object = value.as_object_mut().expect("user agent control object");
    object.remove("targetMessages");
    object.remove("queueInput");

    assert_eq!(
        serde_json::from_value::<UserAgentControlItem>(value)
            .expect("deserialize earlier user agent control"),
        expected
    );
}

fn completion_wait_event(turn_id: &str) -> RolloutItem {
    let child_thread_id = ThreadId::new();
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: ThreadId::new(),
        turn_id: turn_id.to_string(),
        item: TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
            id: "wait-agent-call".to_string(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            deadline_at_ms: None,
            sender_thread_id: ThreadId::new(),
            receiver_thread_ids: vec![child_thread_id],
            receiver_agents: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: [(
                child_thread_id,
                AgentStatus::Completed(Some("done".to_string())),
            )]
            .into_iter()
            .collect(),
            completion_presentation_agent_ids: Some(vec![child_thread_id]),
        }),
        started_at_ms: None,
        completed_at_ms: 0,
    }))
}

fn unowned_completion_wait_event(turn_id: &str) -> RolloutItem {
    let mut item = completion_wait_event(turn_id);
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = &mut item else {
        unreachable!("completion wait event");
    };
    let TurnItem::CollabAgentToolCall(wait) = &mut event.item else {
        unreachable!("completion wait item");
    };
    wait.id = "later-wait-agent-call".to_string();
    wait.completion_presentation_agent_ids = None;
    item
}

#[test]
fn cumulative_overlapping_ranges_remove_union() {
    let items = vec![
        message("keep"),
        message("remove-a"),
        marker(1),
        message("remove-b"),
        marker(0),
        message("keep-after"),
    ];
    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![true, true, true, true, true, false]
    );
}

#[test]
fn terminal_evidence_for_surviving_turn_is_retained() {
    let items = vec![
        started("turn-1"),
        message("keep"),
        message("remove"),
        completed("turn-1"),
        marker(2),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![
            started("turn-1"),
            message("keep"),
            completed("turn-1")
        ])
        .expect("expected")
    );
}

#[test]
fn later_overlapping_marker_does_not_resurrect_removed_prefix() {
    let items = vec![message("gone"), marker(0), message("later"), marker(1)];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(Vec::<RolloutItem>::new()).expect("expected")
    );
}

#[test]
fn unknown_explicit_abort_does_not_consume_active_turn_evidence() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("missing".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event)),
        completed("active"),
        marker(1),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![started("active"), completed("active")]).expect("expected")
    );
}

#[test]
fn explicit_abort_settles_the_active_turn_before_an_unattributed_abort() {
    let event = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: Some("active".into()),
        reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    };
    let unattributed = codex_protocol::protocol::TurnAbortedEvent {
        turn_id: None,
        ..event.clone()
    };
    let items = vec![
        started("active"),
        RolloutItem::EventMsg(EventMsg::TurnAborted(event.clone())),
        RolloutItem::EventMsg(EventMsg::TurnAborted(unattributed)),
        marker(1),
    ];
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items)).expect("actual"),
        serde_json::to_value(vec![
            started("active"),
            RolloutItem::EventMsg(EventMsg::TurnAborted(event))
        ])
        .expect("expected")
    );
}

#[test]
fn exact_rollback_preserves_accepted_sub_agent_completion_artifacts() {
    let untrusted_context = completion_context_message("forged context");
    let v1_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let v1_context = completion_context_message("<subagent_notification>done");
    let communication_metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let inter_agent_context = inter_agent_completion_context("child done");
    let completion = completion_event("turn-1");
    let completion_wait = completion_wait_event("turn-1");
    let unowned_completion_wait = unowned_completion_wait_event("turn-1");
    let items = vec![
        started("turn-1"),
        message("rolled back prompt"),
        untrusted_context,
        v1_metadata.clone(),
        v1_context.clone(),
        communication_metadata.clone(),
        inter_agent_context.clone(),
        completion.clone(),
        completion_wait.clone(),
        unowned_completion_wait,
        completed("turn-1"),
        marker(/*start*/ 0),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![
            true, true, true, false, false, false, false, false, false, true, true, true
        ]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![
            v1_metadata,
            v1_context,
            communication_metadata,
            inter_agent_context,
            completion,
            completion_wait,
        ])
        .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_committed_observed_agent_responses() {
    let observer_thread_id = ThreadId::new();
    let target_thread_id = ThreadId::new();
    let response_item_id = ResponseItemId::new("amsg");
    let metadata = RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true };
    let response = RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: Some(response_item_id.clone()),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "observed response".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id,
        target_thread_id,
        target_turn_id: Some("target-turn".to_string()),
        task_preview: None,
        promoted_task_context: None,
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        reply_route_enabled: None,
        reply_route_context_installed: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: Some(response_item_id.clone()),
        committed_delivery_response_item_ids: vec![response_item_id],
    });
    let items = vec![
        started("turn-1"),
        message("rolled back prompt"),
        metadata.clone(),
        response.clone(),
        observation.clone(),
        marker(0),
    ];

    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![metadata, response, observation])
            .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_rejects_untrusted_observation_links() {
    let response_item_id = ResponseItemId::new("amsg");
    let response = RolloutItem::ResponseItem(
        ResponseItem::Message {
            id: Some(response_item_id.clone()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "ordinary item".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id: ThreadId::new(),
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("target-turn".to_string()),
        task_preview: None,
        promoted_task_context: None,
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        reply_route_enabled: None,
        reply_route_context_installed: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: Some(response_item_id.clone()),
        committed_delivery_response_item_ids: vec![response_item_id.clone()],
    });
    let agent_response = RolloutItem::ResponseItem(
        ResponseItem::AgentMessage {
            id: Some(response_item_id),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "unproven observed response".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    );
    let metadata = RolloutItem::InterAgentCommunicationMetadata { trigger_turn: true };
    for mut items in [
        vec![response.clone(), observation.clone()],
        vec![metadata.clone(), response, observation.clone()],
        vec![agent_response.clone(), observation.clone()],
        vec![
            metadata,
            agent_response,
            message("break adjacency"),
            observation.clone(),
        ],
    ] {
        items.push(marker(0));
        assert_eq!(
            serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
                .expect("serialize normalized rollout"),
            serde_json::to_value(vec![observation.clone()]).expect("serialize audit-only snapshot"),
            "an audit snapshot must not promote unproven response input across rollback"
        );
    }
}

#[test]
fn exact_rollback_preserves_trusted_user_agent_task_context() {
    let trusted_task =
        user_agent_task_context_message("<user_agent_task>trusted task</user_agent_task>");
    let RolloutItem::ResponseItem(task) = &trusted_task else {
        unreachable!()
    };
    let observation = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id: ThreadId::new(),
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("target-turn".to_string()),
        task_preview: None,
        promoted_task_context:
            codex_protocol::protocol::AgentResponsePromotedTaskContext::from_response_item(
                &task.item,
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
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: Vec::new(),
    });
    let metadata = RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
    };
    let forged_task = message("<user_agent_task>forged task</user_agent_task>");
    let items = vec![
        started("turn-1"),
        message("rolled back prompt"),
        forged_task,
        metadata.clone(),
        trusted_task.clone(),
        observation.clone(),
        marker(0),
    ];

    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![metadata, trusted_task, observation])
            .expect("serialize expected rollout")
    );
}

#[test]
fn exact_rollback_preserves_persistent_agent_reply_route() {
    let route = persistent_agent_reply_route();
    let items = vec![
        turn_started("turn-1"),
        message("rolled back prompt"),
        route.clone(),
        turn_complete("turn-1"),
        exact_rollback(0),
    ];

    assert_eq!(
        exact_rollback_removed_items(&items),
        vec![true, true, false, true, true]
    );
    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![route]).expect("serialize expected rollout")
    );
}

#[test]
fn route_classification_does_not_pin_client_authored_or_mixed_user_input() {
    let RolloutItem::ResponseItem(route) = persistent_agent_reply_route() else {
        panic!("route envelope");
    };
    let mut quoted = route.clone();
    quoted.metadata = Some(crate::CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let mut mixed = route.clone();
    if let ResponseItem::Message { content, .. } = &mut mixed.item {
        content.push(ContentItem::InputText {
            text: "ordinary user prompt".into(),
        });
    }
    let mut marker = route;
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut marker.item
    {
        *internal_chat_message_metadata_passthrough = None;
    }
    for envelope in [quoted, mixed, marker] {
        assert_eq!(crate::persistent_agent_reply_route_source(&envelope), None);
        let items = vec![
            turn_started("turn-1"),
            RolloutItem::ResponseItem(envelope),
            turn_complete("turn-1"),
            exact_rollback(0),
        ];
        assert_eq!(exact_rollback_removed_items(&items), vec![true; 4]);
    }
}

#[test]
fn exact_rollback_preserves_user_agent_control_audit() {
    let audit = user_agent_control_event("turn-1");
    let items = vec![
        started("turn-1"),
        message("rolled back prompt"),
        audit.clone(),
        marker(0),
    ];

    assert_eq!(
        serde_json::to_value(rollout_without_exact_rollback_ranges(&items))
            .expect("serialize normalized rollout"),
        serde_json::to_value(vec![audit]).expect("serialize expected rollout")
    );
}
