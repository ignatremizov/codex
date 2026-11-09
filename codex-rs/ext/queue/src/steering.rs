//! Persist explicit active-turn delivery alongside otherwise ordinary queued user input.

use codex_core::CodexThread;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::TurnStartOptions;
use codex_thread_store::MAX_QUEUE_ITEMS;
use serde_json::Value;

use super::QueuedItem;
use std::collections::HashMap;

use crate::QueueServiceError;
use crate::QueuedItemService;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum QueueDelivery {
    #[default]
    NextTurn,
    Steer,
}

enum Dispatch {
    All,
    Scheduled,
}

pub(super) fn encode_input(
    input: &TurnInput,
    delivery: QueueDelivery,
    metadata: Option<&HashMap<String, String>>,
) -> Result<String, QueueServiceError> {
    let mut payload = serde_json::to_value(input)?;
    if delivery == QueueDelivery::Steer || metadata.is_some() {
        let Some(Value::Object(fields)) = payload.get_mut("UserInput") else {
            return Err(QueueServiceError::InvalidInput);
        };
        // Additive fields preserve the historical TurnInput payload for older readers.
        if delivery == QueueDelivery::Steer {
            fields.insert("queue_steer".to_string(), Value::Bool(true));
        }
        if let Some(metadata) = metadata {
            fields.insert(
                "responsesapi_client_metadata".to_string(),
                serde_json::to_value(metadata)?,
            );
        }
    }
    Ok(serde_json::to_string(&payload)?)
}

pub(super) fn decode_input(
    payload: Value,
) -> Result<(TurnInput, QueueDelivery), serde_json::Error> {
    let delivery = if payload
        .get("UserInput")
        .and_then(|input| input.get("queue_steer"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        QueueDelivery::Steer
    } else {
        QueueDelivery::NextTurn
    };
    Ok((serde_json::from_value(payload)?, delivery))
}

impl QueuedItemService {
    /// Returns whether an unaccepted steer needs another dispatch attempt.
    pub(super) async fn dispatch_steers(
        &self,
        thread: &CodexThread,
    ) -> Result<bool, QueueServiceError> {
        self.dispatch_steers_inner(thread, Dispatch::All).await
    }

    /// A due explicit prompt can start an interrupted thread; ordinary queued
    /// input retains its existing wait-for-resume behavior.
    pub(super) async fn dispatch_scheduled(
        &self,
        thread: &CodexThread,
    ) -> Result<bool, QueueServiceError> {
        self.dispatch_steers_inner(thread, Dispatch::Scheduled)
            .await
    }

    async fn dispatch_steers_inner(
        &self,
        thread: &CodexThread,
        dispatch: Dispatch,
    ) -> Result<bool, QueueServiceError> {
        let thread_id = thread.startup_metadata().thread_id;
        let _guard = self.dispatch_guard(thread_id).await;
        let mut pending = false;
        // Explicit steers can pass ordinary next-turn work without consuming it.
        for record in self
            .queue
            .list_page(thread_id, /*offset*/ 0, MAX_QUEUE_ITEMS)
            .await?
        {
            if matches!(dispatch, Dispatch::Scheduled) && record.schedule.is_none() {
                continue;
            }
            if record
                .schedule
                .as_ref()
                .is_some_and(|schedule| schedule.scheduled_at > super::unix_now())
            {
                continue;
            }
            let scheduled = record.schedule.is_some();
            let queued_item_id = record.id.clone();
            let item = match QueuedItem::try_from(record) {
                Ok(item) => item,
                Err(error) => {
                    tracing::warn!(queued_item_id = %queued_item_id, %error, "discarding invalid queued item");
                    self.delete_locked(thread_id, queued_item_id).await?;
                    continue;
                }
            };
            if item.delivery != QueueDelivery::Steer {
                continue;
            }
            // Receipt-only Accepted is specific to inter-agent messages. Keep this
            // user-queue boundary typed so those messages never enter this dispatch path.
            let input @ TurnInput::UserInput { .. } = item.input else {
                return Err(QueueServiceError::InvalidInput);
            };
            match thread
                .start_or_steer_turn(
                    TurnInputRequest::new(input)
                        .with_responses_metadata(item.responsesapi_client_metadata)
                        .on_start(TurnStartOptions {
                            turn_trigger: Some("queue".to_string()),
                            ..Default::default()
                        }),
                )
                .await
            {
                Ok(TurnInputSubmission::Started { turn_id, .. }) => {
                    self.finish_dispatch(
                        thread_id,
                        queued_item_id,
                        turn_id,
                        scheduled,
                        /*started*/ true,
                    )
                    .await?;
                }
                Ok(TurnInputSubmission::Steered { turn_id, .. }) => {
                    self.finish_dispatch(
                        thread_id,
                        queued_item_id,
                        turn_id,
                        scheduled,
                        /*started*/ false,
                    )
                    .await?;
                }
                Ok(TurnInputSubmission::Accepted) => {
                    // Like Core's typed start-if-idle and steer-only APIs, this is an
                    // internal contract violation, not a receiving-turn receipt to invent
                    // or a rejection that can be safely resubmitted.
                    unreachable!("queued user input must start, steer, or decline")
                }
                Ok(TurnInputSubmission::NotSubmitted { reason }) => {
                    tracing::debug!(%thread_id, ?reason, "queued steer deferred by core");
                    pending = true;
                    break;
                }
                Err(error) => {
                    tracing::warn!(%thread_id, %error, "failed to deliver queued steer");
                    pending = true;
                    break;
                }
            }
        }
        Ok(pending)
    }
}
