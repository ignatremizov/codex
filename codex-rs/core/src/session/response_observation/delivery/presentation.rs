use crate::agent::control::CompletionPresentation;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;

pub(super) fn records(
    thread_id: ThreadId,
    active_turn_id: Option<&str>,
    presentation: CompletionPresentation,
) -> (Vec<RolloutItem>, Vec<Event>) {
    let history_only = active_turn_id.is_none();
    let turn_id = active_turn_id.map_or(presentation.history_only_turn_id, str::to_owned);
    let completed = ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.clone(),
        item: presentation.item.clone(),
        started_at_ms: None,
        completed_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
    };
    let mut records = Vec::new();
    if history_only {
        records.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
            TurnStartedEvent {
                turn_id: turn_id.clone(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            },
        )));
    }
    records.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
        completed.clone(),
    )));
    if history_only {
        records.push(RolloutItem::EventMsg(EventMsg::TurnComplete(
            TurnCompleteEvent {
                turn_id: turn_id.clone(),
                last_agent_message: None,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            },
        )));
    }
    let events = vec![
        Event {
            id: turn_id.clone(),
            msg: EventMsg::ItemStarted(ItemStartedEvent {
                thread_id,
                turn_id: turn_id.clone(),
                item: presentation.item,
                started_at_ms: completed.completed_at_ms,
            }),
        },
        Event {
            id: turn_id,
            msg: EventMsg::ItemCompleted(completed),
        },
    ];
    (records, events)
}
