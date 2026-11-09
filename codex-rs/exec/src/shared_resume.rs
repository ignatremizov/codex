//! Final output for queue-routed resume. No hosting-session subscription is retained.

use crate::exec_events::AgentMessageItem;
use crate::exec_events::ItemCompletedEvent;
use crate::exec_events::ThreadErrorEvent;
use crate::exec_events::ThreadEvent;
use crate::exec_events::ThreadItem;
use crate::exec_events::ThreadItemDetails;
use crate::exec_events::ThreadStartedEvent;
use crate::exec_events::TurnCompletedEvent;
use crate::exec_events::TurnFailedEvent;
use crate::exec_events::TurnStartedEvent;
use codex_app_server_protocol::ThreadItemPayload as ApiThreadItem;
use codex_app_server_protocol::ThreadQueueResultResponse;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::models::MessagePhase;
use std::io::Write;
use std::path::Path;

/// Print standard exec final events/output from the exact receiving turn.
/// Progress events and token usage unavailable through history are not replayed.
pub fn write_shared_turn_result(
    result: &ThreadQueueResultResponse,
    json: bool,
    output: Option<&Path>,
) -> anyhow::Result<()> {
    let turn = result
        .turn
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("receiving turn history is unavailable"))?;
    let final_message = turn
        .items
        .iter()
        .rev()
        .find_map(|item| match &item.payload {
            ApiThreadItem::AgentMessage {
                id,
                text,
                phase: None | Some(MessagePhase::FinalAnswer),
                inter_agent_source: None,
                attribution: None,
                ..
            } => Some((id, text)),
            _ => None,
        });
    if final_message.is_none() {
        eprintln!(
            "No final assistant message is available for receiving turn {}.",
            turn.id
        );
    }
    let mut stdout = std::io::stdout().lock();
    if json {
        let mut events = vec![
            ThreadEvent::ThreadStarted(ThreadStartedEvent {
                thread_id: result
                    .thread_id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("missing receiving thread ID"))?,
            }),
            ThreadEvent::TurnStarted(TurnStartedEvent::default()),
        ];
        if let Some((id, text)) = final_message {
            events.push(ThreadEvent::ItemCompleted(ItemCompletedEvent {
                item: ThreadItem {
                    id: id.clone(),
                    details: ThreadItemDetails::AgentMessage(AgentMessageItem {
                        text: text.clone(),
                    }),
                },
            }));
        }
        match turn.status {
            TurnStatus::Completed => events.push(ThreadEvent::TurnCompleted(TurnCompletedEvent {
                usage: Default::default(),
            })),
            TurnStatus::Failed | TurnStatus::Interrupted => {
                events.push(ThreadEvent::TurnFailed(TurnFailedEvent {
                    error: ThreadErrorEvent {
                        message: turn
                            .error
                            .as_ref()
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| format!("receiving turn {:?}", turn.status)),
                    },
                }))
            }
            TurnStatus::InProgress => anyhow::bail!("receiving turn is still in progress"),
        }
        for event in events {
            serde_json::to_writer(&mut stdout, &event)?;
            writeln!(stdout)?;
        }
    } else if let Some((_, text)) = final_message {
        writeln!(stdout, "{text}")?;
    }
    if let Some(output) = output {
        std::fs::write(
            output,
            final_message
                .map(|(_, text)| text.as_str())
                .unwrap_or_default(),
        )?;
    }
    match turn.status {
        TurnStatus::Completed => Ok(()),
        TurnStatus::Failed | TurnStatus::Interrupted => {
            anyhow::bail!("receiving turn {} ended as {:?}", turn.id, turn.status)
        }
        TurnStatus::InProgress => anyhow::bail!("receiving turn is still in progress"),
    }
}
