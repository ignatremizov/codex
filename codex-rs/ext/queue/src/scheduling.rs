//! Timed input uses normal queue dispatch and a small receiving-turn receipt.

use super::*;
use codex_thread_store::QueuedSubmissionReceipt;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64)
}

pub(super) fn validate_scheduled_text(input: &TurnInput) -> Result<(), QueueServiceError> {
    let TurnInput::UserInput { content, .. } = input else {
        return Err(QueueServiceError::InvalidInput);
    };
    if content
        .iter()
        .all(|item| matches!(item, UserInput::Text { .. }))
        && content
            .iter()
            .any(|item| matches!(item, UserInput::Text { text, .. } if !text.trim().is_empty()))
    {
        Ok(())
    } else {
        Err(QueueServiceError::InvalidInput)
    }
}

impl QueuedItemService {
    pub async fn schedule(
        &self,
        thread_id: ThreadId,
        input: TurnInput,
        schedule: QueuedSubmissionSchedule,
    ) -> Result<QueuedItem, QueueServiceError> {
        validate_scheduled_text(&input)?;
        let input = prepare_queued_user_input(input).await?;
        let TurnInput::UserInput {
            client_id: Some(id),
            ..
        } = &input
        else {
            return Err(QueueServiceError::InvalidInput);
        };
        if id.is_empty() {
            return Err(QueueServiceError::InvalidInput);
        }
        let payload = encode_input(&input, QueueDelivery::Steer)?;
        let item = {
            let _guard = self.dispatch_guard(thread_id).await;
            queued_item_from_record(
                self.queue
                    .enqueue_scheduled(thread_id, id.clone(), payload, schedule)
                    .await?,
            )?
        };
        self.emit_changed(thread_id);
        self.wake_if_loaded(thread_id).await;
        Ok(item)
    }

    pub async fn scheduled_threads(
        &self,
        profile: String,
        after: Option<String>,
        limit: usize,
    ) -> Result<Vec<ThreadId>, QueueServiceError> {
        Ok(self
            .queue
            .scheduled_threads(profile, unix_now(), after, limit)
            .await?)
    }

    pub async fn pending_submission(
        &self,
        id: String,
    ) -> Result<Option<(ThreadId, QueuedItem)>, QueueServiceError> {
        self.queue
            .get_submission(id)
            .await?
            .map(|record| Ok((record.thread_id, queued_item_from_record(record)?)))
            .transpose()
    }

    pub async fn receipt(
        &self,
        id: String,
    ) -> Result<Option<QueuedSubmissionReceipt>, QueueServiceError> {
        Ok(self.queue.submission_receipt(id).await?)
    }

    pub async fn note_dispatch_error(
        &self,
        thread_id: ThreadId,
        profile: &str,
        message: String,
    ) -> Result<(), QueueServiceError> {
        for item in self.list(thread_id).await? {
            if item.schedule.as_ref().is_some_and(|schedule| {
                schedule.scheduled_at <= unix_now() && schedule.fallback_profile == profile
            }) {
                self.queue
                    .set_dispatch_error(item.id, message.clone())
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn finish_dispatch(
        &self,
        thread_id: ThreadId,
        id: String,
        turn_id: String,
        scheduled: bool,
        started: bool,
    ) -> Result<(), QueueServiceError> {
        if scheduled {
            self.queue
                .acknowledge_submission(QueuedSubmissionReceipt {
                    id,
                    thread_id,
                    turn_id,
                    started,
                })
                .await?;
            self.emit_changed(thread_id);
        } else {
            self.delete_locked(thread_id, id).await?;
        }
        Ok(())
    }
}
