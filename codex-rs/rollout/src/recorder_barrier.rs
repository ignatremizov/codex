//! Single-attempt canonical writes and recovery of the next append position.

use std::io::Error as IoError;

use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::JsonlWriter;
use super::RolloutCmd;
use super::RolloutRecorder;
use super::RolloutWriterState;
use super::open_log_file;
use super::open_rollout_for_append;
use crate::RolloutItem;
use crate::RolloutLine;

impl RolloutRecorder {
    /// Append one canonical item and complete its durability barrier as one writer command.
    ///
    /// Session metadata must already be persisted; this command never creates a rollout or writes
    /// its first record.
    ///
    /// The item is kept out of the normal retry queue. If writing fails before the item is
    /// complete, a later flush cannot retry that item. An error still means unknown durability:
    /// bytes may have reached the file before the barrier failed. Accepted commands continue
    /// even when the caller stops waiting.
    pub async fn record_canonical_item_and_flush(&self, item: &RolloutItem) -> std::io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::AddItemAndFlush {
                item: Box::new(item.clone()),
                ack: tx,
            })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout item and flush: {e}"))
                })
            })?;
        rx.await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!(
                    "failed waiting for rollout item durability barrier: {e}"
                ))
            })
        })?
    }
}

impl RolloutWriterState {
    pub(super) async fn add_item_and_flush(&mut self, item: RolloutItem) -> std::io::Result<()> {
        // Keep this command away from initial metadata writes, then drain older work before
        // writing the item outside `pending_items`. A failure is ambiguous, but must never arrange
        // a second attempt behind the caller's back.
        if self.meta.is_some() {
            return Err(IoError::other(
                "single-item durability barrier requires persisted session metadata",
            ));
        }
        self.flush().await?;
        self.ensure_writer_open().await?;
        if let Some(writer) = &self.writer
            && writer.file.metadata().await?.len() == 0
        {
            return Err(IoError::other(
                "single-item durability barrier requires persisted session metadata",
            ));
        }

        let ordinal = self.ordinal_state.current()?;
        let write_result = match self.writer.as_mut() {
            Some(writer) => match writer.write_rollout_item_buffered(&item, ordinal).await {
                Ok(()) => {
                    self.ordinal_state.advance();
                    writer.file.flush().await
                }
                Err(err) => Err(err),
            },
            None => Err(IoError::other("rollout writer is not open")),
        };
        if let Err(err) = &write_result {
            self.enter_recovery_mode(err);
        }
        write_result
    }

    pub(super) async fn ensure_writer_open(&mut self) -> std::io::Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        if self.deferred_creation {
            let file = open_log_file(self.rollout_path.as_path())?;
            self.writer = Some(JsonlWriter {
                file: tokio::fs::File::from_std(file),
            });
            self.deferred_creation = false;
        } else if self.meta.is_some() {
            // Metadata remains pending when its first write reports an error. A complete JSON
            // record may nevertheless be present (for example when only its trailing newline or
            // flush failed); recognize that commit instead of duplicating ordinal zero. Any
            // incomplete initial fragment can be rewritten. Never truncate other complete records.
            let contents = tokio::fs::read(self.rollout_path.as_path()).await?;
            let mut records = contents
                .split(|byte| *byte == b'\n')
                .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()));
            let first_record = records.next();
            let complete_session_meta = match first_record {
                Some(line) => match crate::parse_rollout_line_bytes(line) {
                    Ok(RolloutLine {
                        item: RolloutItem::SessionMeta(metadata),
                        ..
                    }) if self
                        .meta
                        .as_ref()
                        .is_some_and(|pending| pending.id == metadata.meta.id) =>
                    {
                        true
                    }
                    Ok(_) => {
                        return Err(IoError::other(format!(
                            "rollout at {} does not start with session metadata",
                            self.rollout_path.display()
                        )));
                    }
                    Err(_) => false,
                },
                None => false,
            };
            if complete_session_meta {
                let (_path, file, ordinal_state) =
                    open_rollout_for_append(self.rollout_path.as_path(), self.writer_lock.clone())
                        .await?;
                self.writer = Some(JsonlWriter { file });
                self.ordinal_state = ordinal_state;
                self.meta = None;
            } else {
                if records.next().is_some() {
                    return Err(IoError::other(
                        "cannot repair initial metadata over existing rollout records",
                    ));
                }
                tokio::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(self.rollout_path.as_path())
                    .await?;
                let file = open_log_file(self.rollout_path.as_path())?;
                self.writer = Some(JsonlWriter {
                    file: tokio::fs::File::from_std(file),
                });
            }
        } else {
            // A failed write can leave a complete final JSON object without its newline. Reopening
            // repairs that terminator and must derive the next ordinal from the repaired tail;
            // retaining the pre-write ordinal would let the next append reuse a committed ordinal.
            let (_path, file, ordinal_state) =
                open_rollout_for_append(self.rollout_path.as_path(), self.writer_lock.clone())
                    .await?;
            self.writer = Some(JsonlWriter { file });
            self.ordinal_state = ordinal_state;
        }
        Ok(())
    }
}
