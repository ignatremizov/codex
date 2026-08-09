use super::*;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::rollout::rollout_without_exact_rollback_ranges;
use std::borrow::Cow;
use std::collections::HashSet;

pub(in crate::session) fn initial_agent_response_observation_state(
    initial_history: &InitialHistory,
) -> AgentResponseObservationState {
    let mut state = AgentResponseObservationState::default();
    let rollout_items = initial_history.get_rollout_items();
    let canonical_items = if rollout_items
        .iter()
        .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
    {
        Cow::Owned(rollout_without_exact_rollback_ranges(rollout_items))
    } else {
        Cow::Borrowed(rollout_items)
    };
    for response_event in agent_response_events_from_rollout(&canonical_items) {
        state.next_event_sequence = state.next_event_sequence.wrapping_add(1);
        apply_agent_response_event_state(&mut state, &response_event);
    }
    state
}

pub(crate) fn agent_response_events_from_rollout(
    rollout_items: &[RolloutItem],
) -> Vec<AgentResponseEvent> {
    let completed_commentary_items = rollout_items
        .iter()
        .filter_map(|item| {
            let RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) = item else {
                return None;
            };
            let TurnItem::AgentMessage(item) = &event.item else {
                return None;
            };
            matches!(item.phase.as_ref(), Some(MessagePhase::Commentary))
                .then(|| (event.turn_id.clone(), item.id.clone()))
        })
        .collect::<HashSet<_>>();
    let mut next_sequence = 0_u64;
    let mut response_events = Vec::new();
    for item in rollout_items {
        let response_event = match item {
            RolloutItem::EventMsg(event) => agent_response_event(event, next_sequence),
            RolloutItem::ResponseItem(response_item) => {
                let turn_id = response_item.turn_id().map(ToOwned::to_owned);
                match &response_item.item {
                    ResponseItem::Message {
                        id: Some(item_id),
                        role,
                        content,
                        phase: Some(MessagePhase::Commentary),
                        ..
                    } if role == "assistant"
                        && turn_id.as_ref().is_some_and(|turn_id| {
                            !completed_commentary_items
                                .contains(&(turn_id.clone(), item_id.to_string()))
                        }) =>
                    {
                        turn_id.map(|turn_id| AgentResponseEvent::Commentary {
                            turn_id,
                            item_id: item_id.to_string(),
                            text: content
                                .iter()
                                .filter_map(|content| match content {
                                    ContentItem::InputText { text }
                                    | ContentItem::OutputText { text } => Some(text.as_str()),
                                    ContentItem::InputImage { .. }
                                    | ContentItem::InputAudio { .. } => None,
                                })
                                .collect(),
                            sequence: next_sequence,
                        })
                    }
                    ResponseItem::AdditionalTools { .. }
                    | ResponseItem::Message { .. }
                    | ResponseItem::AgentMessage { .. }
                    | ResponseItem::Reasoning { .. }
                    | ResponseItem::LocalShellCall { .. }
                    | ResponseItem::FunctionCall { .. }
                    | ResponseItem::ToolSearchCall { .. }
                    | ResponseItem::FunctionCallOutput { .. }
                    | ResponseItem::CustomToolCall { .. }
                    | ResponseItem::CustomToolCallOutput { .. }
                    | ResponseItem::ToolSearchOutput { .. }
                    | ResponseItem::WebSearchCall { .. }
                    | ResponseItem::ImageGenerationCall { .. }
                    | ResponseItem::Compaction { .. }
                    | ResponseItem::CompactionTrigger { .. }
                    | ResponseItem::ContextCompaction { .. }
                    | ResponseItem::Other => None,
                }
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::AgentResponseObservation(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::SecurityRiskScore(_)
            | RolloutItem::TokenUsageRecord(_)
            | RolloutItem::RealtimeItem(_) => None,
        };
        if let Some(response_event) = response_event {
            response_events.push(response_event);
            next_sequence = next_sequence.wrapping_add(1);
        }
    }
    response_events
}
