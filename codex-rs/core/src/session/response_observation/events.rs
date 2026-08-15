//! Live and recovered response events share one turn-state reducer.

use super::*;

pub(super) fn begin_agent_response_turn_locked(
    state: &mut AgentResponseObservationState,
    turn_id: &str,
) -> bool {
    state.live_turn_id = Some(turn_id.to_string());
    if state.active_turn_id.as_deref() == Some(turn_id) {
        return false;
    }
    state.active_turn_id = Some(turn_id.to_string());
    state.latest_admitted_turn_id = Some(turn_id.to_string());
    state.last_terminal = None;
    state.last_commentary_item_id = None;
    true
}

pub(super) fn publish_agent_response_event_locked(
    state: &mut AgentResponseObservationState,
    response_event: AgentResponseEvent,
) {
    state.next_event_sequence = state.next_event_sequence.wrapping_add(1);
    apply_agent_response_event_state(state, &response_event);
    state
        .subscribers
        .retain(|_, subscriber| subscriber.sender.send(response_event.clone()).is_ok());
}

pub(super) fn apply_agent_response_event_state(
    state: &mut AgentResponseObservationState,
    response_event: &AgentResponseEvent,
) {
    match &response_event {
        AgentResponseEvent::TurnStarted { turn_id, .. } => {
            state.active_turn_id = Some(turn_id.clone());
            state.latest_admitted_turn_id = Some(turn_id.clone());
            state.last_terminal = None;
            state.last_commentary_item_id = None;
        }
        AgentResponseEvent::Terminal {
            turn_id, status, ..
        } => {
            let status = state
                .terminal_outcomes
                .entry(turn_id.clone())
                .or_insert_with(|| status.clone())
                .clone();
            if state.latest_admitted_turn_id.is_none() {
                state.latest_admitted_turn_id = Some(turn_id.clone());
            }
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.active_turn_id = None;
                state.last_commentary_item_id = None;
            }
            // A prior turn can finish after a newer turn has started. Preserve that terminal
            // outcome while the newer turn remains active, but never let a delayed historical
            // outcome overwrite the final snapshot of a newer completed turn.
            if state.active_turn_id.is_some()
                || state.latest_admitted_turn_id.as_deref() == Some(turn_id.as_str())
            {
                state.last_terminal = Some((turn_id.clone(), status));
            }
        }
        AgentResponseEvent::TurnAborted { turn_id, .. } => {
            if state.latest_admitted_turn_id.is_none() {
                state.latest_admitted_turn_id = Some(turn_id.clone());
            }
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.active_turn_id = None;
                state.last_commentary_item_id = None;
            }
        }
        AgentResponseEvent::Commentary {
            turn_id, item_id, ..
        } => {
            if state.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                state.last_commentary_item_id = Some(item_id.clone());
            }
        }
    }
}

pub(super) fn agent_response_event(event: &EventMsg, sequence: u64) -> Option<AgentResponseEvent> {
    match event {
        EventMsg::TurnStarted(event) => Some(AgentResponseEvent::TurnStarted {
            turn_id: event.turn_id.clone(),
            sequence,
        }),
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(item)
                if matches!(item.phase.as_ref(), Some(MessagePhase::Commentary))
                    && !item.has_sub_agent_completion_identity()
                    && !item.is_attributed_agent_input_presentation() =>
            {
                Some(AgentResponseEvent::Commentary {
                    turn_id: event.turn_id.clone(),
                    item_id: item.id.clone(),
                    text: agent_message_text(item),
                    sequence,
                })
            }
            _ => None,
        },
        EventMsg::TurnComplete(event) => {
            agent_status_from_event(&EventMsg::TurnComplete(event.clone())).map(|status| {
                AgentResponseEvent::Terminal {
                    turn_id: event.turn_id.clone(),
                    status,
                }
            })
        }
        EventMsg::TurnAborted(event) => {
            let turn_id = event.turn_id.clone()?;
            match agent_status_from_event(&EventMsg::TurnAborted(event.clone())) {
                Some(status) if is_final(&status) => {
                    Some(AgentResponseEvent::Terminal { turn_id, status })
                }
                Some(_) => Some(AgentResponseEvent::TurnAborted { turn_id }),
                None => None,
            }
        }
        _ => None,
    }
}
